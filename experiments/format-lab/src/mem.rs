//! Resident set size, read from the kernel.
//!
//! One of the M1 questions is what a dictionary build costs in memory, and the honest answer to that
//! is what the operating system says the process is holding, not what a counter inside the program
//! thinks it allocated. Those two numbers differ by whatever the allocator is keeping in its free
//! lists, which for a build that grows a hash table by doubling is most of a doubling.
//!
//! Linux only. The measurement hosts are Linux and every other platform returns `None` rather than
//! a guess.

/// Current resident set size in bytes.
pub fn rss() -> Option<usize> {
    field("VmRSS:")
}

/// The high water mark of the resident set size in bytes, which is the number a build that has
/// already finished has to be judged on.
pub fn peak_rss() -> Option<usize> {
    field("VmHWM:")
}

#[cfg(target_os = "linux")]
fn field(name: &str) -> Option<usize> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix(name) {
            // The kernel writes these as "VmRSS:\t   12345 kB" and has done since forever.
            let kb: usize = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn field(_name: &str) -> Option<usize> {
    None
}
