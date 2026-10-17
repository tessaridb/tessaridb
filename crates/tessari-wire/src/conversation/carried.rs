//! What a conversation is carried over (ADR-0089).

use tokio::io::{AsyncRead, AsyncWrite, DuplexStream};
use tokio::net::TcpStream;

/// What a conversation is carried over: a TCP socket this node accepted, or a
/// pipe another surface feeds from a WebSocket (ADR-0089).
///
/// The protocol is the same bytes either way, read and written from the
/// connection's task.
pub(crate) trait Carried: Send + Sized + 'static {
    /// The half a conversation reads.
    type Reading: AsyncRead + Unpin + Send + 'static;
    /// The half it writes.
    type Writing: AsyncWrite + Unpin + Send + 'static;

    /// Split into the two halves.
    fn halves(self) -> (Self::Reading, Self::Writing);
}

impl Carried for TcpStream {
    type Reading = tokio::net::tcp::OwnedReadHalf;
    type Writing = tokio::net::tcp::OwnedWriteHalf;

    fn halves(self) -> (Self::Reading, Self::Writing) {
        self.into_split()
    }
}

impl Carried for DuplexStream {
    type Reading = tokio::io::ReadHalf<Self>;
    type Writing = tokio::io::WriteHalf<Self>;

    fn halves(self) -> (Self::Reading, Self::Writing) {
        tokio::io::split(self)
    }
}

/// TLS on a socket this node accepted (ADR-0108 D4).
impl Carried for tokio_rustls::server::TlsStream<TcpStream> {
    type Reading = tokio::io::ReadHalf<Self>;
    type Writing = tokio::io::WriteHalf<Self>;

    fn halves(self) -> (Self::Reading, Self::Writing) {
        tokio::io::split(self)
    }
}
