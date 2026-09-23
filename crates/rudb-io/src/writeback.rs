//! Starting the write of what a file has been given, without waiting for it.
//!
//! A bulk load writes a gigabyte or more through the page cache and then syncs the file once at
//! the commit. The sync is then the whole file going to the device at once, after every thread
//! has stopped encoding, and on the 32 core box a ClickBench `hits` 10M load spent 1.3 to 1.7 s of
//! its 8 to 9 s there. Asking the kernel to start writing each stretch as soon as it is complete
//! moves that work under the encode, and the sync at the end only waits for the tail.
//!
//! This is a hint. Nothing is durable because of it, the sync at the commit is still what makes a
//! file durable, and where there is no such call it does nothing.

use std::fs::File;

/// Asks the kernel to start writing `length` bytes of `file` from `offset` to the device, and
/// returns without waiting for them.
///
/// On Linux this is `sync_file_range` with `SYNC_FILE_RANGE_WRITE` only. It may still wait when
/// the device's queue is full, which is the queue telling the writer it is ahead of the disk.
/// Errors are ignored, because the sync at the commit reports every one that matters.
pub fn start_writeback(file: &File, offset: u64, length: u64) {
    imp::start(file, offset, length);
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[allow(unsafe_code, reason = "sync_file_range has no wrapper in std")]
mod imp {
    use std::ffi::{c_int, c_uint};
    use std::fs::File;
    use std::os::fd::AsRawFd;

    unsafe extern "C" {
        fn sync_file_range(fd: c_int, offset: i64, nbytes: i64, flags: c_uint) -> c_int;
    }

    const SYNC_FILE_RANGE_WRITE: c_uint = 2;

    pub(super) fn start(file: &File, offset: u64, length: u64) {
        let (Ok(offset), Ok(length)) = (i64::try_from(offset), i64::try_from(length)) else {
            return;
        };
        // SAFETY: the descriptor is open for as long as `file` is borrowed, and the call takes
        // three integers by value and touches no memory of ours.
        let _ = unsafe { sync_file_range(file.as_raw_fd(), offset, length, SYNC_FILE_RANGE_WRITE) };
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
mod imp {
    use std::fs::File;

    pub(super) fn start(_: &File, _: u64, _: u64) {}
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::start_writeback;

    #[test]
    fn starting_the_writeback_leaves_the_bytes_as_they_were() {
        let path = std::env::temp_dir().join(format!("rudb-writeback-{}", std::process::id()));
        let mut file = std::fs::File::create(&path).expect("a file");
        file.write_all(&[7u8; 1 << 16]).expect("written");
        start_writeback(&file, 0, 1 << 16);
        // Past the end and zero long are both allowed and both nothing.
        start_writeback(&file, 1 << 20, 0);
        file.sync_all().expect("synced");
        assert_eq!(std::fs::read(&path).expect("read"), vec![7u8; 1 << 16]);
        std::fs::remove_file(path).expect("removed");
    }
}
