//! A cursor over the body of one message.
//!
//! Each method fails the way the `pq_getmsg*` function of the same name in
//! `src/backend/libpq/pqformat.c` fails, with the same text. All of them are [`Level::Error`]:
//! the frame was read in full, so the server is still in sync with the client.
//!
//! [`Level::Error`]: crate::Level::Error

use crate::error::ProtocolError;

#[derive(Debug, Clone)]
pub(crate) struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Reader<'a> {
        Reader { data, at: 0 }
    }

    pub(crate) fn remaining(&self) -> usize {
        self.data.len() - self.at
    }

    /// `pq_getmsgbyte`.
    pub(crate) fn byte(&mut self) -> Result<u8, ProtocolError> {
        let Some(&byte) = self.data.get(self.at) else {
            return Err(ProtocolError::error("no data left in message"));
        };
        self.at += 1;
        Ok(byte)
    }

    /// `pq_copymsgbytes`, which is under `pq_getmsgint`.
    fn array<const N: usize>(&mut self) -> Result<[u8; N], ProtocolError> {
        let bytes = self.take(N)?;
        let mut array = [0; N];
        array.copy_from_slice(bytes);
        Ok(array)
    }

    /// `pq_getmsgint(msg, 2)`. PostgreSQL reads the value as unsigned.
    pub(crate) fn u16(&mut self) -> Result<u16, ProtocolError> {
        self.array().map(u16::from_be_bytes)
    }

    pub(crate) fn i16(&mut self) -> Result<i16, ProtocolError> {
        self.array().map(i16::from_be_bytes)
    }

    /// `pq_getmsgint(msg, 4)`.
    pub(crate) fn u32(&mut self) -> Result<u32, ProtocolError> {
        self.array().map(u32::from_be_bytes)
    }

    pub(crate) fn i32(&mut self) -> Result<i32, ProtocolError> {
        self.array().map(i32::from_be_bytes)
    }

    /// `pq_getmsgbytes`. A length below zero fails the same way as a length past the end.
    pub(crate) fn bytes(&mut self, len: i32) -> Result<&'a [u8], ProtocolError> {
        match usize::try_from(len) {
            Ok(len) => self.take(len),
            Err(_) => Err(ProtocolError::error("insufficient data left in message")),
        }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], ProtocolError> {
        if len > self.remaining() {
            return Err(ProtocolError::error("insufficient data left in message"));
        }
        let bytes = &self.data[self.at..self.at + len];
        self.at += len;
        Ok(bytes)
    }

    /// `pq_getmsgstring`, without the conversion from the client encoding, which is not the job
    /// of the codec. The string does not include its terminating zero byte.
    pub(crate) fn string(&mut self) -> Result<&'a [u8], ProtocolError> {
        let rest = &self.data[self.at..];
        let Some(len) = rest.iter().position(|&b| b == 0) else {
            return Err(ProtocolError::error("invalid string in message"));
        };
        self.at += len + 1;
        Ok(&rest[..len])
    }

    /// The bytes that are left, which is the whole of the rest of the message.
    pub(crate) fn rest(&mut self) -> &'a [u8] {
        let rest = &self.data[self.at..];
        self.at = self.data.len();
        rest
    }

    /// `pq_getmsgend`.
    pub(crate) fn end(&self) -> Result<(), ProtocolError> {
        if self.remaining() == 0 {
            Ok(())
        } else {
            Err(ProtocolError::error("invalid message format"))
        }
    }
}
