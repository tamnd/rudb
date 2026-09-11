//! How much memory the machine has, which is where the default budget comes from.
//!
//! `spec/05-storage.md` section 5 says the buffer pool is the memory limit and that there is one
//! budget rather than two, and `spec/04-architecture.md` section 4 says an operator that goes
//! around it makes the limit a lie. Neither of them says what the limit is when nobody sets one,
//! and until this module existed the answer was that there was none, so the first thing to stop a
//! runaway query was the allocator's abort handler and the process went with it.
//!
//! DuckDB defaults to eighty percent of physical memory. Being a drop in replacement means a query
//! that fits in one fits in the other, so the fraction is copied rather than chosen, and the number
//! it is a fraction of has to be read the same way, which is per platform and without a crate.
//!
//! Three platforms and one honest failure. Linux reads `/proc/meminfo`, macOS asks `sysctl` for
//! `hw.memsize`, Windows calls `GlobalMemoryStatusEx`, and anything else says it does not know. A
//! machine that does not know gets no default limit, which is what every machine got before, so the
//! unsupported case is no worse than the state this replaces.
//!
//! On Linux the answer is the smaller of what the machine has and what the control group allows.
//! `/proc/meminfo` inside a container reports the host, so a two gigabyte container on a two
//! hundred gigabyte host would take a hundred and sixty gigabyte budget and be killed by the kernel
//! long before the budget noticed. CI runs in containers, so getting this wrong means the default
//! is wrong exactly where it is least watched.
//!
//! It lives in this crate rather than next to the budget in `rudb-common` for the two reasons this
//! crate exists. Asking the operating system how much memory it has is the same kind of question as
//! asking it for a file, and on Linux it is literally a file read, which the rule at the top of
//! this crate says goes through here. The other reason is that `rudb-common` forbids unsafe code
//! and two of the three platforms answer through a C call.

/// What the machine has, in bytes, or `None` when there is no way to ask on this platform.
///
/// On Linux this is the smaller of the physical total and the control group limit, because a
/// process in a container can only have what the container allows and `/proc/meminfo` does not know
/// that.
#[must_use]
pub fn physical_memory() -> Option<u64> {
    platform::physical_memory()
}

/// The fraction of the machine a database takes when nobody says otherwise.
///
/// Eighty percent, which is DuckDB's, and the twenty that is left is for everything this budget
/// does not count: the allocator's own bookkeeping, the page cache the scan reads through, and
/// every other process on the machine. [`rudb_common::Memory`] has the list of what is counted,
/// which is what the operators reserved and not the resident size of the process.
pub const DEFAULT_FRACTION: f64 = 0.8;

/// The budget a database opens with on this machine, or `None` when the machine will not say.
///
/// Rounded down to a whole mebibyte, which is not about the memory and is about the printing.
/// `--print-config` writes a size with a unit only when the byte count divides by it exactly, so
/// four fifths of a machine to the byte comes out as `27487790694B`, which nobody reads. A budget
/// does not need byte precision and the megabyte that is dropped is nothing next to the fifth of
/// the machine that is already being left alone.
#[must_use]
pub fn default_memory_limit() -> Option<u64> {
    const MIB: u64 = 1 << 20;
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a byte count of a real machine is far under the 2^53 a double holds exactly, \
                  and the product of a positive count and a positive fraction under one is \
                  positive and smaller than the count"
    )]
    let share = |bytes: u64| (bytes as f64 * DEFAULT_FRACTION) as u64 / MIB * MIB;
    physical_memory().map(share).filter(|&bytes| bytes > 0)
}

#[cfg(target_os = "linux")]
mod platform {
    /// The smaller of what the kernel says the machine has and what the control group allows.
    pub(super) fn physical_memory() -> Option<u64> {
        let total = meminfo(&std::fs::read_to_string("/proc/meminfo").ok()?)?;
        Some(cgroup().map_or(total, |limit| total.min(limit)))
    }

    /// `MemTotal` out of the text of `/proc/meminfo`, which is in kibibytes and says so.
    ///
    /// Split out from the read so there is something to test. The file is not on every machine this
    /// compiles for, and a test that skips on the machine it was written on is a test nobody runs.
    fn meminfo(text: &str) -> Option<u64> {
        let line = text.lines().find(|line| line.starts_with("MemTotal:"))?;
        let mut fields = line.split_whitespace().skip(1);
        let value: u64 = fields.next()?.parse().ok()?;
        match fields.next() {
            Some("kB") => value.checked_mul(1024),
            None => Some(value),
            Some(_) => None,
        }
    }

    /// What the control group allows, if there is one and it has a number rather than `max`.
    ///
    /// Version two first, because a machine with both mounted is running version two and version
    /// one is there for compatibility. A group with no limit writes `max` in version two and a
    /// number close to `u64::MAX` in version one, and both of those mean the same as no file at
    /// all.
    fn cgroup() -> Option<u64> {
        const V1: &str = "/sys/fs/cgroup/memory/memory.limit_in_bytes";
        const V2: &str = "/sys/fs/cgroup/memory.max";
        for at in [V2, V1] {
            let Ok(text) = std::fs::read_to_string(at) else { continue };
            let text = text.trim();
            if text == "max" {
                return None;
            }
            let Ok(bytes) = text.parse::<u64>() else { continue };
            // Version one writes a number the size of the address space to mean no limit, and it is
            // page aligned rather than exactly `u64::MAX`, so the test is an order of magnitude and
            // not equality. No machine has an exabyte.
            if bytes >= 1 << 60 {
                return None;
            }
            return Some(bytes);
        }
        None
    }

    #[cfg(test)]
    mod tests {
        use super::meminfo;

        #[test]
        fn memtotal_is_read_in_kibibytes() {
            let text = "MemTotal:       32773868 kB\nMemFree:         4113928 kB\n";
            assert_eq!(meminfo(text), Some(32_773_868 * 1024));
        }

        #[test]
        fn a_file_without_memtotal_is_no_answer_rather_than_a_wrong_one() {
            assert_eq!(meminfo("MemFree: 4113928 kB\n"), None);
            assert_eq!(meminfo(""), None);
        }

        #[test]
        fn a_unit_this_does_not_know_is_refused() {
            // The file has said kB since it was written and this is the line that notices if that
            // ever changes, rather than reading a gibibyte count as a kibibyte one.
            assert_eq!(meminfo("MemTotal: 32 GB\n"), None);
        }

        #[test]
        fn the_machine_this_runs_on_says_something_sensible() {
            let bytes = super::physical_memory().expect("a Linux box knows how much memory it has");
            assert!(bytes >= 1 << 28, "no machine builds this in under 256 MiB, got {bytes}");
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
#[allow(unsafe_code, reason = "the only way to ask this platform is a C call into sysctl")]
mod platform {
    use std::ffi::{c_char, c_int, c_void};

    unsafe extern "C" {
        fn sysctlbyname(
            name: *const c_char,
            oldp: *mut c_void,
            oldlenp: *mut usize,
            newp: *mut c_void,
            newlen: usize,
        ) -> c_int;
    }

    /// `hw.memsize`, which is the byte count of installed memory.
    ///
    /// There is no control group question here. A container on macOS is a Linux virtual machine and
    /// the code inside it is the Linux branch above.
    pub(super) fn physical_memory() -> Option<u64> {
        let name = c"hw.memsize";
        let mut bytes: u64 = 0;
        let mut size = size_of::<u64>();
        // SAFETY: `name` is a nul terminated string with a lifetime longer than the call, `bytes`
        // is a live `u64` and `size` says so, and the new value pointer is null with a length of
        // zero, which is how this call is told to read rather than write. The return code is
        // checked, so a failure leaves `bytes` at the zero it was initialised with and is reported
        // as no answer.
        let rc = unsafe {
            sysctlbyname(
                name.as_ptr(),
                std::ptr::from_mut(&mut bytes).cast::<c_void>(),
                &raw mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 || size != size_of::<u64>() || bytes == 0 {
            return None;
        }
        Some(bytes)
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn the_machine_this_runs_on_says_something_sensible() {
            let bytes = super::physical_memory().expect("a Mac knows how much memory it has");
            assert!(bytes >= 1 << 28, "no machine builds this in under 256 MiB, got {bytes}");
        }
    }
}

#[cfg(target_os = "windows")]
#[allow(unsafe_code, reason = "the only way to ask this platform is a call into kernel32")]
mod platform {
    /// `MEMORYSTATUSEX`, laid out the way `windows.h` lays it out.
    ///
    /// Only the first three fields are read. The rest are here because the call writes all of them
    /// and a shorter struct would be written past the end of.
    #[repr(C)]
    struct MemoryStatusEx {
        length: u32,
        memory_load: u32,
        total_physical: u64,
        available_physical: u64,
        total_page_file: u64,
        available_page_file: u64,
        total_virtual: u64,
        available_virtual: u64,
        available_extended_virtual: u64,
    }

    unsafe extern "system" {
        fn GlobalMemoryStatusEx(buffer: *mut MemoryStatusEx) -> i32;
    }

    /// `ullTotalPhys`, which is the byte count of physical memory the process can see.
    pub(super) fn physical_memory() -> Option<u64> {
        let mut status = MemoryStatusEx {
            length: u32::try_from(size_of::<MemoryStatusEx>()).ok()?,
            memory_load: 0,
            total_physical: 0,
            available_physical: 0,
            total_page_file: 0,
            available_page_file: 0,
            total_virtual: 0,
            available_virtual: 0,
            available_extended_virtual: 0,
        };
        // SAFETY: the pointer is to a live, fully initialised struct of exactly the layout the call
        // expects, and `length` has been set to its size, which is how the call is told which
        // version of the struct it was given. A zero return means it wrote nothing and is reported
        // as no answer rather than as the zeroes the struct started with.
        let ok = unsafe { GlobalMemoryStatusEx(&raw mut status) };
        if ok == 0 || status.total_physical == 0 {
            return None;
        }
        Some(status.total_physical)
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn the_machine_this_runs_on_says_something_sensible() {
            let bytes = super::physical_memory().expect("Windows knows how much memory it has");
            assert!(bytes >= 1 << 28, "no machine builds this in under 256 MiB, got {bytes}");
        }
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "windows"
)))]
mod platform {
    /// Nothing, on a platform nobody has written the call for.
    ///
    /// A guess would be worse than no answer. The caller turns this into no default limit, which is
    /// what every platform had before this module, so the port is no worse off than it was.
    pub(super) fn physical_memory() -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_FRACTION, default_memory_limit, physical_memory};

    #[test]
    fn the_default_is_four_fifths_of_what_the_machine_has() {
        let Some(bytes) = physical_memory() else {
            eprintln!("skipping, this platform does not say how much memory it has");
            return;
        };
        let limit = default_memory_limit().expect("a machine that says its size has a default");
        assert!(limit < bytes, "the default leaves room for everything the budget does not count");
        #[expect(clippy::cast_precision_loss, reason = "a byte count is far under 2^53")]
        let want = (bytes as f64 * DEFAULT_FRACTION) as u64;
        assert!(want - limit < (1 << 20), "{limit} is a mebibyte or less under {want}");
    }

    #[test]
    fn the_default_is_a_whole_number_of_mebibytes_so_that_it_prints_as_one() {
        let Some(limit) = default_memory_limit() else {
            eprintln!("skipping, this platform does not say how much memory it has");
            return;
        };
        assert_eq!(limit % (1 << 20), 0, "{limit} would print as a byte count");
    }

    #[test]
    fn asking_twice_gives_the_same_answer() {
        // It is read at open time and printed by `--print-config`, so a number that moved between
        // the two would be a configuration that does not describe the database it came from.
        assert_eq!(physical_memory(), physical_memory());
    }
}
