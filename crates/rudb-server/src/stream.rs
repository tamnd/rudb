//! A connection on TCP, on TLS over TCP or on a Unix socket, as one type for the session.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;

use crate::tls::TlsStream;

/// The socket of one client.
#[derive(Debug)]
pub(crate) enum Stream {
    Tcp(TcpStream),
    Unix(UnixStream),
    Tls(Box<TlsStream>),
}

impl Stream {
    /// A second handle on the same socket, so that the server can shut it down from another
    /// thread.
    pub(crate) fn try_clone(&self) -> io::Result<Stream> {
        Ok(match self {
            Stream::Tcp(s) => Stream::Tcp(s.try_clone()?),
            Stream::Unix(s) => Stream::Unix(s.try_clone()?),
            Stream::Tls(s) => Stream::Tcp(s.sock.try_clone()?),
        })
    }

    /// True when a read gives bytes or the end without a wait on the socket: TLS can hold
    /// plaintext that it decrypted from the last read, which `poll(2)` does not see.
    pub(crate) fn buffered(&mut self) -> bool {
        match self {
            Stream::Tls(s) => match s.conn.process_new_packets() {
                Ok(state) => state.plaintext_bytes_to_read() > 0 || state.peer_has_closed(),
                // The read reports the error.
                Err(_) => true,
            },
            Stream::Tcp(_) | Stream::Unix(_) => false,
        }
    }

    /// Ends both directions, which wakes a thread that waits on the socket.
    pub(crate) fn shutdown(&self) {
        // The socket can already be closed by the client, and then there is nothing to do.
        let _ = match self {
            Stream::Tcp(s) => s.shutdown(Shutdown::Both),
            Stream::Unix(s) => s.shutdown(Shutdown::Both),
            Stream::Tls(s) => s.sock.shutdown(Shutdown::Both),
        };
    }
}

impl AsRawFd for Stream {
    fn as_raw_fd(&self) -> RawFd {
        match self {
            Stream::Tcp(s) => s.as_raw_fd(),
            Stream::Unix(s) => s.as_raw_fd(),
            Stream::Tls(s) => s.sock.as_raw_fd(),
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Tcp(s) => s.read(buf),
            Stream::Unix(s) => s.read(buf),
            Stream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Tcp(s) => s.write(buf),
            Stream::Unix(s) => s.write(buf),
            Stream::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Tcp(_) | Stream::Unix(_) => Ok(()),
            Stream::Tls(s) => s.flush(),
        }
    }
}
