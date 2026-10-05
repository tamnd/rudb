//! `poll(2)` over a few file descriptors, the one call of the server that waits.
//!
//! A session waits on its socket and on its wake pipe, and the acceptor waits on its listeners and
//! on its stop pipe. So one thread sleeps until there is input or until another thread needs it.

use std::io;
use std::os::fd::RawFd;

/// Waits until one of `fds` can be read, and gives the readiness of each one in the same order.
/// A descriptor that hung up or has an error is ready too, because the read then tells what
/// happened.
///
/// # Errors
///
/// An error of `poll` other than `EINTR`, which only starts the wait again.
pub(crate) fn readable<const N: usize>(fds: [RawFd; N]) -> io::Result<[bool; N]> {
    let mut polled = fds.map(|fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 });
    wait(&mut polled)?;
    Ok(polled.map(ready))
}

/// [`readable`] for a list of any length.
///
/// # Errors
///
/// An error of `poll` other than `EINTR`.
pub(crate) fn readable_any(fds: &[RawFd]) -> io::Result<Vec<bool>> {
    let mut polled: Vec<libc::pollfd> =
        fds.iter().map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
    wait(&mut polled)?;
    Ok(polled.into_iter().map(ready).collect())
}

fn wait(polled: &mut [libc::pollfd]) -> io::Result<()> {
    loop {
        // SAFETY: `polled` is a valid buffer of `polled.len()` entries for the whole call.
        let n = unsafe { libc::poll(polled.as_mut_ptr(), polled.len() as libc::nfds_t, -1) };
        if n >= 0 {
            return Ok(());
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

/// Fills `buf` with strong random bytes from the system.
///
/// # Panics
///
/// When the system has no random source, which is not a state in which the server can make a
/// cancel key that is safe.
pub(crate) fn random<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    for part in buf.chunks_mut(256) {
        // SAFETY: `part` is a valid buffer of `part.len()` bytes, and `getentropy` takes at most
        // 256 bytes in one call.
        let done = unsafe { libc::getentropy(part.as_mut_ptr().cast(), part.len()) };
        assert_eq!(done, 0, "getentropy failed: {}", io::Error::last_os_error());
    }
    buf
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
    }
}
