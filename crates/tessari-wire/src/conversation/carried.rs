//! What a conversation is carried over (ADR-0089).

use tokio::io::{AsyncRead, AsyncWrite, DuplexStream};
use tokio::net::TcpStream;

use crate::error::{Error, Result};

/// What a conversation is carried over: a TCP socket this node accepted, or a
/// pipe another surface feeds from a WebSocket (ADR-0089).
///
/// The protocol is the same bytes either way. What differs is whether a busy
/// connection can be handed to a store thread (`hot.rs`), which blocks on a real
/// socket and so exists only for TCP.
pub(crate) trait Carried: Send + Sized + 'static {
    /// The half a conversation reads.
    type Reading: AsyncRead + Unpin + Send + 'static;
    /// The half it writes.
    type Writing: AsyncWrite + Unpin + Send + 'static;
    /// Whether a busy connection may be served on a store thread.
    const THREADED: bool;

    /// Split into the two halves.
    fn halves(self) -> (Self::Reading, Self::Writing);

    /// Put the halves back together as a blocking socket, for `hot.rs`.
    ///
    /// Called only when [`Carried::THREADED`] holds.
    fn to_thread(reading: Self::Reading, writing: Self::Writing) -> Result<std::net::TcpStream>;

    /// The halves again, from the socket a store thread gave back.
    fn from_thread(stream: std::net::TcpStream) -> Result<(Self::Reading, Self::Writing)>;
}

impl Carried for TcpStream {
    type Reading = tokio::net::tcp::OwnedReadHalf;
    type Writing = tokio::net::tcp::OwnedWriteHalf;
    const THREADED: bool = true;

    fn halves(self) -> (Self::Reading, Self::Writing) {
        self.into_split()
    }

    fn to_thread(reading: Self::Reading, writing: Self::Writing) -> Result<std::net::TcpStream> {
        let stream = reading
            .reunite(writing)
            .map_err(|_| Error::Io(std::io::Error::other("a connection's halves parted")))?
            .into_std()?;
        stream.set_nonblocking(false)?;
        Ok(stream)
    }

    fn from_thread(stream: std::net::TcpStream) -> Result<(Self::Reading, Self::Writing)> {
        stream.set_nonblocking(true)?;
        Ok(Self::from_std(stream)?.into_split())
    }
}

impl Carried for DuplexStream {
    type Reading = tokio::io::ReadHalf<Self>;
    type Writing = tokio::io::WriteHalf<Self>;
    const THREADED: bool = false;

    fn halves(self) -> (Self::Reading, Self::Writing) {
        tokio::io::split(self)
    }

    fn to_thread(_: Self::Reading, _: Self::Writing) -> Result<std::net::TcpStream> {
        Err(Error::Io(std::io::Error::other(
            "a pipe has no socket to hand to a store thread",
        )))
    }

    fn from_thread(_: std::net::TcpStream) -> Result<(Self::Reading, Self::Writing)> {
        Err(Error::Io(std::io::Error::other(
            "a pipe has no socket to take back from a store thread",
        )))
    }
}
