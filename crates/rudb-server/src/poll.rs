//! `poll(2)` over a few file descriptors, the one call of the server that waits.
//!
//! A session waits on its socket and on its wake pipe, and the acceptor waits on its listeners and
//! on its stop pipe. So one thread sleeps until there is input or until another thread needs it.

use std::io;
use std::os::fd::RawFd;
use std::sync::{Mutex, PoisonError};

/// Waits until one of `fds` can be read, and gives the readiness of each one in the same order.
/// A descriptor that hung up or has an error is ready too, because the read then tells what
/// happened.
///
/// # Errors
///
/// An error of `poll` other than `EINTR`, which only starts the wait again.
pub(crate) fn readable<const N: usize>(fds: [RawFd; N]) -> io::Result<[bool; N]> {
    let mut polled = fds.map(|fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 });
    wait(&mut polled, -1)?;
    Ok(polled.map(ready))
}

/// [`readable`] that waits for `millis` milliseconds at most, and gives `None` when no
/// descriptor is ready in that time.
///
/// # Errors
///
/// An error of `poll` other than `EINTR`.
pub(crate) fn readable_within<const N: usize>(
    fds: [RawFd; N],
    millis: i32,
) -> io::Result<Option<[bool; N]>> {
    let mut polled = fds.map(|fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 });
    // A wait that a signal stops starts again with all of its time, so it can be longer than
    // `millis`, which does not matter to the callers.
    let ready_count = wait(&mut polled, millis)?;
    Ok((ready_count > 0).then(|| polled.map(ready)))
}

/// [`readable`] for a list of any length.
///
/// # Errors
///
/// An error of `poll` other than `EINTR`.
pub(crate) fn readable_any(fds: &[RawFd]) -> io::Result<Vec<bool>> {
    let mut polled: Vec<libc::pollfd> =
        fds.iter().map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
    wait(&mut polled, -1)?;
    Ok(polled.into_iter().map(ready).collect())
}

/// The number of ready descriptors, zero when `millis` passed first. A negative `millis` waits
/// with no limit.
fn wait(polled: &mut [libc::pollfd], millis: i32) -> io::Result<usize> {
    loop {
        // SAFETY: `polled` is a valid buffer of `polled.len()` entries for the whole call.
        let n = unsafe { libc::poll(polled.as_mut_ptr(), polled.len() as libc::nfds_t, millis) };
        if let Ok(n) = usize::try_from(n) {
            return Ok(n);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn ready(polled: libc::pollfd) -> bool {
    polled.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
}

/// Gives back to the system the pages of the stack of the calling thread below the frame of the
/// caller. A statement can use tens of KiB of stack, and the pages stay with the thread after the
/// statement ends. An idle session calls this so that it holds only the pages that it uses while
/// it waits. The next statement gets new pages, which are zero, as it goes deeper again.
///
/// It does nothing on a system other than Linux and macOS.
#[inline(never)]
pub(crate) fn trim_stack() {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let Some(low) = stack_low() else { return };
        let here = std::hint::black_box(0u8);
        let here = std::ptr::from_ref(&here) as usize;
        // SAFETY: `sysconf` has no preconditions.
        let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap_or(0);
        if page == 0 {
            return;
        }
        // The page of this frame and one page more stay, for the frame of `madvise` itself.
        let end = here.saturating_sub(page) & !(page - 1);
        let start = low.next_multiple_of(page);
        if end <= start {
            return;
        }
        // SAFETY: the range is in the stack of this thread, below the frame of this function and
        // a page more, so no frame that is live uses it. The pages stay mapped and writable, and
        // a later frame that touches one gets a page of zeros, which is what a frame expects of
        // nothing.
        unsafe {
            #[cfg(target_os = "linux")]
            libc::madvise(start as *mut libc::c_void, end - start, libc::MADV_DONTNEED);
            // On macOS, `madvise` keeps the pages resident, so new pages of zeros go over them.
            #[cfg(target_os = "macos")]
            libc::mmap(
                start as *mut libc::c_void,
                end - start,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_FIXED,
                -1,
                0,
            );
        }
    }
}

/// The lowest address of the stack of the calling thread, above its guard page.
#[cfg(target_os = "linux")]
fn stack_low() -> Option<usize> {
    let mut attr = std::mem::MaybeUninit::<libc::pthread_attr_t>::uninit();
    // SAFETY: `pthread_getattr_np` fills `attr` when it gives 0, and `attr` is destroyed once.
    unsafe {
        if libc::pthread_getattr_np(libc::pthread_self(), attr.as_mut_ptr()) != 0 {
            return None;
        }
        let mut addr = std::ptr::null_mut();
        let mut size = 0;
        let got = libc::pthread_attr_getstack(attr.as_ptr(), &raw mut addr, &raw mut size);
        libc::pthread_attr_destroy(attr.as_mut_ptr());
        (got == 0 && !addr.is_null()).then_some(addr as usize)
    }
}

/// The lowest address of the stack of the calling thread, above its guard page.
#[cfg(target_os = "macos")]
fn stack_low() -> Option<usize> {
    // SAFETY: both calls only read the description of the calling thread.
    let (top, size) = unsafe {
        let me = libc::pthread_self();
        (libc::pthread_get_stackaddr_np(me) as usize, libc::pthread_get_stacksize_np(me))
    };
    top.checked_sub(size)
}

/// Strong random bytes from the system.
///
/// The bytes come from a pool that one call of `getentropy` fills, 256 bytes at a time, and each
/// byte goes out once. So a new connection, which needs a cancel key, does not make a system call
/// of its own each time. A request for more than the pool holds goes to the system directly.
///
/// # Panics
///
/// When the system has no random source, which is not a state in which the server can make a
/// cancel key that is safe.
pub(crate) fn random<const N: usize>() -> [u8; N] {
    static POOL: Mutex<Pool> = Mutex::new(Pool { left: 0, bytes: [0; POOL_LEN] });
    let mut buf = [0u8; N];
    if N > POOL_LEN {
        for part in buf.chunks_mut(POOL_LEN) {
            entropy(part);
        }
        return buf;
    }
    let mut pool = POOL.lock().unwrap_or_else(PoisonError::into_inner);
    if pool.left < N {
        entropy(&mut pool.bytes);
        pool.left = POOL_LEN;
    }
    let from = POOL_LEN - pool.left;
    buf.copy_from_slice(&pool.bytes[from..from + N]);
    // The bytes that went out are not kept, so a later reader of the pool cannot see them.
    pool.bytes[from..from + N].fill(0);
    pool.left -= N;
    buf
}

/// The most bytes that `getentropy` gives in one call.
const POOL_LEN: usize = 256;

/// The random bytes not handed out yet, which are the last `left` bytes of `bytes`.
struct Pool {
    left: usize,
    bytes: [u8; POOL_LEN],
}

fn entropy(part: &mut [u8]) {
    // SAFETY: `part` is a valid buffer of `part.len()` bytes, and the callers give at most 256
    // bytes, which is the most that `getentropy` takes in one call.
    let done = unsafe { libc::getentropy(part.as_mut_ptr().cast(), part.len()) };
    assert_eq!(done, 0, "getentropy failed: {}", io::Error::last_os_error());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    #[test]
    fn only_the_descriptor_with_input_is_ready() {
        let (mut a, b) = UnixStream::pair().unwrap();
        let (c, _d) = UnixStream::pair().unwrap();
        a.write_all(b"x").unwrap();
        assert_eq!(readable([b.as_raw_fd(), c.as_raw_fd()]).unwrap(), [true, false]);
        assert_eq!(readable_any(&[c.as_raw_fd(), b.as_raw_fd()]).unwrap(), [false, true]);
        drop(a);
        assert_eq!(readable([b.as_raw_fd()]).unwrap(), [true]);
    }

    #[test]
    fn random_bytes_differ() {
        assert_ne!(random::<32>(), random::<32>());
        assert_eq!(random::<300>().len(), 300);
        // More than the pool holds, taken in small parts, still differ from part to part.
        let parts: Vec<[u8; 24]> = (0..40).map(|_| random::<24>()).collect();
        for (at, part) in parts.iter().enumerate() {
            assert!(parts[at + 1..].iter().all(|other| other != part));
        }
    }
}
