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
//! The group to ask is the one this process is in, named in `/proc/self/cgroup`, and every parent
//! of it up to the mount root, because a limit on a parent binds a child as much as its own does.
//! Reading the mount root alone is the mistake #223 was: it is the right file only under Docker,
//! where the mount is namespaced so that the root is the container's own group, and it is the wrong
//! file for a systemd scope, for Kubernetes and for anything nested.
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

    /// The smallest limit binding this process, across both hierarchy versions.
    ///
    /// A machine may have either mounted or both, so both are asked and the answer is the smaller.
    /// Version two is the one that is used on a modern machine and version one is there because a
    /// long lived host may still be on it.
    fn cgroup() -> Option<u64> {
        let own = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
        let v2 = smallest("/sys/fs/cgroup", v2_path(&own), "memory.max");
        let v1 = smallest("/sys/fs/cgroup/memory", v1_path(&own), "memory.limit_in_bytes");
        match (v2, v1) {
            (Some(two), Some(one)) => Some(two.min(one)),
            (two, one) => two.or(one),
        }
    }

    /// The group this process is in under the version two hierarchy.
    ///
    /// One line, `0::` and then the path, which is what the single hierarchy of version two means.
    fn v2_path(own: &str) -> &str {
        own.lines().find_map(|line| line.strip_prefix("0::")).unwrap_or("")
    }

    /// The group this process is in under the memory controller of the version one hierarchy.
    ///
    /// One line per controller, numbered, and the controller field holds a comma separated list
    /// because one hierarchy can carry several. Only the one carrying `memory` says anything about
    /// memory.
    fn v1_path(own: &str) -> &str {
        own.lines()
            .find_map(|line| {
                let mut fields = line.splitn(3, ':');
                let controllers = fields.nth(1)?;
                let path = fields.next()?;
                controllers.split(',').any(|name| name == "memory").then_some(path)
            })
            .unwrap_or("")
    }

    /// The smallest limit from the group at `path` up through every parent to `mount`.
    ///
    /// Every ancestor is read rather than only the group the process is in, because a parent's
    /// limit binds its children as much as their own does, and systemd routinely puts a limit on a
    /// slice rather than on the scope inside it.
    ///
    /// The mount root is read too, and it is read even when `path` names a group that is not there.
    /// Inside a container `/proc/self/cgroup` reports the path on the host, which does not exist
    /// under the namespaced mount, and the mount root is the container's own group. So the walk
    /// finds nothing and the last read is the one that answers.
    fn smallest(mount: &str, path: &str, file: &str) -> Option<u64> {
        let mut parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
        let mut smallest: Option<u64> = None;
        loop {
            let mut at = std::path::PathBuf::from(mount);
            at.extend(&parts);
            at.push(file);
            if let Some(bytes) = limit(&at) {
                smallest = Some(smallest.map_or(bytes, |had| had.min(bytes)));
            }
            if parts.pop().is_none() {
                return smallest;
            }
        }
    }

    /// One limit file, or `None` when it is absent, unreadable or says there is no limit.
    ///
    /// A group with no limit writes `max` in version two and a number the size of the address space
    /// in version one. The version one number is page aligned rather than exactly `u64::MAX`, so the
    /// test is an order of magnitude rather than equality. No machine has an exabyte.
    fn limit(at: &std::path::Path) -> Option<u64> {
        let text = std::fs::read_to_string(at).ok()?;
        let bytes: u64 = text.trim().parse().ok()?;
        (bytes < 1 << 60).then_some(bytes)
    }

    #[cfg(test)]
    mod tests {
        use super::{limit, meminfo, smallest, v1_path, v2_path};
        use crate::scratch::TempDir;

        #[test]
        fn the_version_two_group_is_the_one_line_that_names_no_controller() {
            let own = "0::/system.slice/run-r867.scope\n";
            assert_eq!(v2_path(own), "/system.slice/run-r867.scope");
            assert_eq!(v2_path("11:memory:/docker/abc\n"), "");
            assert_eq!(v2_path(""), "");
        }

        #[test]
        fn the_version_one_group_is_the_line_whose_controllers_include_memory() {
            let own = "12:pids:/user.slice\n11:memory:/docker/abc\n0::/\n";
            assert_eq!(v1_path(own), "/docker/abc");
        }

        #[test]
        fn a_controller_named_memory_is_not_one_whose_name_merely_contains_it() {
            // `hugetlb,memory` is a real pairing and `memory_recursiveprot` is a real mount option,
            // so the field is split on commas and matched whole rather than searched for.
            assert_eq!(v1_path("9:hugetlb,memory:/here\n"), "/here");
            assert_eq!(v1_path("9:memory_pressure:/elsewhere\n"), "");
        }

        #[test]
        fn a_group_with_no_limit_says_nothing_rather_than_a_number() {
            let dir = TempDir::new("cgroup-none");
            let at = dir.join("memory.max");
            std::fs::write(&at, "max\n").expect("a file to read back");
            assert_eq!(limit(&at), None);
            // Version one's way of saying the same thing, page aligned rather than u64::MAX.
            std::fs::write(&at, "9223372036854771712\n").expect("a file to read back");
            assert_eq!(limit(&at), None);
            std::fs::write(&at, "12884901888\n").expect("a file to read back");
            assert_eq!(limit(&at), Some(12_884_901_888));
            assert_eq!(limit(&dir.join("no-such-file")), None);
        }

        #[test]
        fn the_walk_takes_the_smallest_limit_on_the_way_up() {
            let mount = TempDir::new("cgroup-walk");
            let deep = mount.join("system.slice/run.scope");
            std::fs::create_dir_all(&deep).expect("a temporary hierarchy");
            // A slice held to eight gigabytes with a scope inside it held to twelve. The scope's
            // own file is the larger number and the one that binds is the parent's.
            std::fs::write(deep.join("memory.max"), "12884901888").expect("a file");
            std::fs::write(mount.join("system.slice/memory.max"), "8589934592").expect("a file");
            std::fs::write(mount.join("memory.max"), "max").expect("a file");
            let root = mount.path().to_str().expect("a path this test wrote");
            let walked = smallest(root, "/system.slice/run.scope", "memory.max");
            assert_eq!(walked, Some(8_589_934_592));
            // A group that is not there is the container case, where the mount root answers. It
            // says `max` here, so what this asserts is that a missing group is not an error and
            // does not stop the walk before it reaches the root.
            assert_eq!(smallest(root, "/docker/abc", "memory.max"), None);
            std::fs::write(mount.join("memory.max"), "2147483648").expect("a file");
            assert_eq!(smallest(root, "/docker/abc", "memory.max"), Some(2_147_483_648));
        }

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
